//! Connection sidebar and the new/edit connection modal.

use std::collections::{HashMap, HashSet};
use std::path::PathBuf;

use eframe::egui;

use crate::app::{
    AppState, Connection, ConnectionState, Environment, FolderNode, SavedQuery,
    SavedQueryVersion, SidebarView, connection_folder_tree, normalize_folder_path,
    saved_folder_tree,
};
use crate::db::{
    ConnectionConfig, ConnectionId, DbError, DbKind, MysqlSslMode, PostgresSslMode,
    parse_connection_url,
};
use crate::ssh::{SshAuth, SshConfig};

use super::{Palette, Theme};

/// A connection's state as the card and the dot read it: one value that
/// derives both the painted shape and the color, so a dot can never wear the
/// wrong color for its state. `Error` keeps its own variant: a failed
/// connection is a standing state, so it wears the softened Guard Red, never
/// the raw Fault Red the design reserves for terminal text.
#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum ConnectionStatus {
    Connected,
    Connecting,
    Failed,
    Offline,
}

impl ConnectionStatus {
    fn of(state: &ConnectionState) -> Self {
        match state {
            ConnectionState::Connected { .. } | ConnectionState::QueryRunning { .. } => {
                Self::Connected
            }
            ConnectionState::Connecting { .. } => Self::Connecting,
            ConnectionState::Error { .. } => Self::Failed,
            ConnectionState::Disconnected => Self::Offline,
        }
    }

    fn color(self, palette: &Palette) -> egui::Color32 {
        match self {
            Self::Connected => palette.success,
            Self::Connecting => egui::Color32::YELLOW,
            Self::Failed => palette.badge_prod,
            Self::Offline => egui::Color32::GRAY,
        }
    }

    /// The state's name, for the dot's node so a screen reader (and a frame
    /// test) can name the mark.
    fn label(self) -> &'static str {
        match self {
            Self::Connected => "connected",
            Self::Connecting => "connecting",
            Self::Failed => "failed",
            Self::Offline => "offline",
        }
    }
}

/// A plain filled disc, for the history rows: a run's outcome is not one of
/// the connection states, so it carries its own color.
fn filled_dot(ui: &mut egui::Ui, color: egui::Color32, size: f32) {
    let (rect, _) = ui.allocate_exact_size(egui::Vec2::splat(size), egui::Sense::hover());
    ui.painter().circle_filled(rect.center(), size / 2.0, color);
}

/// The stroke width of the ring a state dot paints, in points.
const DOT_RING: f32 = 1.5;

/// The painted connection state dot: a filled disc for a connected
/// connection, a ring for an offline one, a half-filled disc for one that is
/// connecting. The glyphs it replaces (`●`, `○`, `◌`) have no glyph in the
/// embedded fonts and painted as epaint's replacement box.
pub(crate) fn state_dot(
    ui: &mut egui::Ui,
    status: ConnectionStatus,
    size: f32,
) -> egui::Response {
    let (rect, response) =
        ui.allocate_exact_size(egui::Vec2::splat(size), egui::Sense::hover());
    let enabled = ui.is_enabled();
    response.widget_info(|| {
        egui::WidgetInfo::labeled(egui::WidgetType::Other, enabled, status.label())
    });
    let center = rect.center();
    let radius = size / 2.0;
    let color = status.color(&Palette::of(ui.ctx().theme()));
    let painter = ui.painter();
    let ring = egui::Stroke::new(DOT_RING, color);
    // The ring is drawn inside the allocated square, so the dot never paints
    // over the control beside it.
    let ring_radius = radius - DOT_RING / 2.0;
    match status {
        ConnectionStatus::Connected => {
            painter.circle_filled(center, radius, color);
        }
        ConnectionStatus::Offline | ConnectionStatus::Failed => {
            painter.circle_stroke(center, ring_radius, ring);
        }
        ConnectionStatus::Connecting => {
            painter.circle_stroke(center, ring_radius, ring);
            painter.add(egui::Shape::convex_polygon(
                vec![
                    center + egui::vec2(0.0, -ring_radius),
                    center + egui::vec2(0.0, ring_radius),
                    center + egui::vec2(ring_radius, 0.0),
                ],
                color,
                egui::Stroke::NONE,
            ));
        }
    }
    response
}

enum SidebarAction {
    Connect(ConnectionId),
    Disconnect(ConnectionId),
    NewQuery(ConnectionId),
    Edit(ConnectionId),
    Delete(ConnectionId),
    RefreshSchema(ConnectionId),
    PreviewTable(ConnectionId, String, String),
    RevealTable(ConnectionId, String, String),
    TableCount(ConnectionId, String, String),
    ToggleBookmark(ConnectionId, String, String),
    RefreshSessions(ConnectionId),
    TerminateSession(ConnectionId, String),
    OpenHistory(ConnectionId, String),
    ClearHistory,
    OpenSaved(u64),
    DeleteSaved(u64),
    RestoreSavedVersion(u64, usize),
    OpenSavedVersion(u64, usize),
    MoveSaved(u64, Option<String>),
    MoveConnection(ConnectionId, Option<String>),
    NewConnectionInFolder(String),
}

/// Drag payload for a saved query row. A distinct type from [`ConnDrag`] so
/// a query can only land on saved folders, never on connection folders.
#[derive(Clone)]
struct SavedDrag {
    id: u64,
}

/// Drag payload for a connection card, landing on connection folders only.
#[derive(Clone, Copy)]
struct ConnDrag {
    id: ConnectionId,
}

/// Temp-data id for one folder's collapse state.
fn folder_open_id(scope: &str, path: &str) -> egui::Id {
    egui::Id::new(format!("crusty_{scope}_folder_open#{path}"))
}

/// Whether one tree folder renders expanded. Defaults to open.
fn folder_is_open(ctx: &egui::Context, scope: &str, path: &str) -> bool {
    let id = folder_open_id(scope, path);
    ctx.data_mut(|d| d.get_temp(id)).unwrap_or(true)
}

/// Flip one tree folder's collapse state.
fn set_folder_open(ctx: &egui::Context, scope: &str, path: &str, open: bool) {
    let id = folder_open_id(scope, path);
    ctx.data_mut(|d| d.insert_temp(id, open));
}

/// Temp-data id for the set of freshly created empty folders. Creation names
/// a path before any entry targets it; the set keeps it visible this
/// session, and the first entry moved into it persists it through the
/// entry's own folder string.
fn created_id(scope: &str) -> egui::Id {
    egui::Id::new(format!("crusty_{scope}_created_folders"))
}

/// The freshly created empty folders for one section.
fn created_folders(ctx: &egui::Context, scope: &str) -> HashSet<String> {
    let id = created_id(scope);
    ctx.data_mut(|d| d.get_temp(id).unwrap_or_default())
}

/// Remember one freshly created empty folder for one section.
fn remember_created_folder(ctx: &egui::Context, scope: &str, path: String) {
    let mut created = created_folders(ctx, scope);
    created.insert(path);
    let id = created_id(scope);
    ctx.data_mut(|d| d.insert_temp(id, created));
}

/// Temp-data id for one folder creation input.
fn folder_input_id(scope: &str, parent: &str, part: &str) -> egui::Id {
    egui::Id::new(format!("crusty_{scope}_folder_input#{parent}#{part}"))
}

/// The text of one folder creation input.
fn folder_input(ctx: &egui::Context, scope: &str, parent: &str, part: &str) -> String {
    ctx.data_mut(|d| d.get_temp(folder_input_id(scope, parent, part)).unwrap_or_default())
}

/// Replace the text of one folder creation input.
fn set_folder_input(ctx: &egui::Context, scope: &str, parent: &str, part: &str, text: String) {
    ctx.data_mut(|d| d.insert_temp(folder_input_id(scope, parent, part), text));
}

/// How many entries sit under one tree node, direct and nested, for the
/// folder header count.
fn subtree_count<T>(node: &FolderNode<T>) -> usize {
    node.entries.len() + node.children.iter().map(subtree_count).sum::<usize>()
}

/// One flattened saved row in display order: root entries first, then each
/// folder pre-order with its direct entries before its subfolders.
enum SavedPlan<'a> {
    Folder { path: String, name: String, depth: usize, open: bool, count: usize },
    Entry { query: &'a SavedQuery, depth: usize },
}

/// Flatten one saved tree level into the display plan, skipping the
/// children of collapsed folders.
fn flatten_saved<'s>(
    ctx: &egui::Context,
    node: &FolderNode<&'s SavedQuery>,
    depth: usize,
    plan: &mut Vec<SavedPlan<'s>>,
) {
    for child in &node.children {
        let open = folder_is_open(ctx, "saved", &child.path);
        plan.push(SavedPlan::Folder {
            path: child.path.clone(),
            name: child.name.clone(),
            depth,
            open,
            count: subtree_count(child),
        });
        if open {
            for query in &child.entries {
                plan.push(SavedPlan::Entry { query, depth: depth + 1 });
            }
            flatten_saved(ctx, child, depth + 1, plan);
        }
    }
}

/// One connection as the sidebar card reads it: the status the dot paints
/// and the text beside it, which is the server version, the error, or the
/// state's name.
struct CardEntry {
    id: ConnectionId,
    name: String,
    env: Environment,
    status: ConnectionStatus,
    state_text: String,
}

/// One flattened connection row in display order, mirroring [`SavedPlan`].
enum ConnPlan {
    Folder { path: String, name: String, depth: usize, open: bool, count: usize },
    Card { id: ConnectionId, depth: usize },
}

/// Flatten one connection tree level into the display plan.
fn flatten_connections(
    ctx: &egui::Context,
    node: &FolderNode<&Connection>,
    depth: usize,
    plan: &mut Vec<ConnPlan>,
) {
    for child in &node.children {
        let open = folder_is_open(ctx, "conn", &child.path);
        plan.push(ConnPlan::Folder {
            path: child.path.clone(),
            name: child.name.clone(),
            depth,
            open,
            count: subtree_count(child),
        });
        if open {
            for entry in &child.entries {
                plan.push(ConnPlan::Card { id: entry.id, depth: depth + 1 });
            }
            flatten_connections(ctx, child, depth + 1, plan);
        }
    }
}

/// Render contents indented one step per tree depth.
fn indented<R>(ui: &mut egui::Ui, depth: usize, add_contents: impl FnOnce(&mut egui::Ui) -> R) -> R {
    if depth == 0 {
        add_contents(ui)
    } else {
        ui.horizontal(|ui| {
            ui.add_space(depth as f32 * 14.0);
            ui.vertical(add_contents).inner
        })
        .inner
    }
}

/// Confirm one folder creation input: normalize the typed name under
/// `parent`, remember the path so the empty folder renders, and disarm the
/// input. Blank input only disarms.
fn confirm_folder_input(ctx: &egui::Context, scope: &str, parent: &str) {
    let text = folder_input(ctx, scope, parent, "text");
    let full = if parent.is_empty() { text.clone() } else { format!("{parent}/{text}") };
    if let Some(path) = normalize_folder_path(Some(&full)) {
        remember_created_folder(ctx, scope, path);
    }
    disarm_folder_input(ctx, scope, parent);
}

/// Whether one folder creation input is armed.
fn folder_input_armed(ctx: &egui::Context, scope: &str, parent: &str) -> bool {
    folder_input(ctx, scope, parent, "armed") == "1"
}

/// Arm one folder creation input.
fn arm_folder_input(ctx: &egui::Context, scope: &str, parent: &str) {
    set_folder_input(ctx, scope, parent, "armed", "1".to_owned());
}

/// Disarm one folder creation input without creating anything.
fn disarm_folder_input(ctx: &egui::Context, scope: &str, parent: &str) {
    set_folder_input(ctx, scope, parent, "text", String::new());
    set_folder_input(ctx, scope, parent, "armed", String::new());
}

/// Temp-data id for the SQL import target folder.
fn sql_import_folder_id() -> egui::Id {
    egui::Id::new("crusty_saved_sql_import_folder")
}

/// The SQL import target folder text.
fn sql_import_folder(ctx: &egui::Context) -> String {
    ctx.data_mut(|d| d.get_temp(sql_import_folder_id())).unwrap_or_default()
}

/// Replace the SQL import target folder text.
fn set_sql_import_folder(ctx: &egui::Context, folder: String) {
    ctx.data_mut(|d| d.insert_temp(sql_import_folder_id(), folder));
}

/// Temp-data id for the last SQL import note.
fn sql_import_note_id() -> egui::Id {
    egui::Id::new("crusty_saved_sql_import_note")
}

/// The last SQL import note, if any.
fn sql_import_note_of(ctx: &egui::Context) -> Option<String> {
    ctx.data_mut(|d| d.get_temp(sql_import_note_id()))
}

/// Remember one SQL import outcome as the sidebar note.
fn store_sql_import_note(ctx: &egui::Context, note: String) {
    ctx.data_mut(|d| d.insert_temp(sql_import_note_id(), note));
}

/// Pick `.sql` files and import one saved query per file into `folder`.
fn import_picked_sql_files(state: &mut AppState, ctx: &egui::Context, folder: &str) {
    let Some(paths) = rfd::FileDialog::new().add_filter("SQL", &["sql"]).pick_files() else {
        return;
    };
    store_sql_import_note(ctx, sql_import_note(state, &paths, folder));
}

/// Pick a directory and import every `.sql` file inside it into `folder`.
fn import_picked_sql_dir(state: &mut AppState, ctx: &egui::Context, folder: &str) {
    let Some(dir) = rfd::FileDialog::new().pick_folder() else { return };
    let paths = crate::app::collect_sql_files(&dir);
    let note = if paths.is_empty() {
        format!("Import failed: no .sql files in {}", dir.display())
    } else {
        sql_import_note(state, &paths, folder)
    };
    store_sql_import_note(ctx, note);
}

/// Run one SQL file import and render its outcome as the sidebar note: the
/// imported count with the normalized folder, the skipped file names, or the
/// missing-connection failure. None from [`AppState::import_sql_files`] means
/// no connection exists to attribute the queries to.
fn sql_import_note(
    state: &mut AppState,
    paths: &[std::path::PathBuf],
    folder: &str,
) -> String {
    let Some(outcome) = state.import_sql_files(paths, Some(folder)) else {
        return "Import failed: add a connection first".to_owned();
    };
    let folder = crate::app::normalize_folder_path(Some(folder));
    let into = folder.map(|f| format!(" into {f}")).unwrap_or_default();
    if outcome.imported == 0 {
        if outcome.skipped.is_empty() {
            return "Import failed: no .sql files picked".to_owned();
        }
        return format!("Import failed: skipped {}", outcome.skipped.join(", "));
    }
    if outcome.skipped.is_empty() {
        format!("Imported {} {}{into}", outcome.imported, plural_queries(outcome.imported))
    } else {
        format!(
            "Imported {} {} (skipped {}){into}",
            outcome.imported,
            plural_queries(outcome.imported),
            outcome.skipped.join(", ")
        )
    }
}

/// `1 query` or `n queries` for the import note.
fn plural_queries(n: usize) -> &'static str {
    if n == 1 { "query" } else { "queries" }
}

fn history_title(sql: &str) -> String {
    let first = sql.lines().map(str::trim).find(|l| !l.is_empty()).unwrap_or("SQL");
    let mut title: String = first.chars().take(28).collect();
    if first.chars().count() > 28 {
        title += "…";
    }
    title
}

fn env_badge(ui: &mut egui::Ui, env: Environment) {
    let color = env.badge_color(Theme::from_egui(ui.ctx().theme()));
    egui::Frame::NONE
        .fill(color.gamma_multiply(0.2))
        .stroke(egui::Stroke::new(1.0, color.gamma_multiply(0.6)))
        .corner_radius(egui::CornerRadius::same(3))
        .inner_margin(egui::Margin::symmetric(4, 1))
        .show(ui, |ui| {
            ui.label(egui::RichText::new(env.label()).small().strong().color(color));
        });
}

/// Temp-data id for one saved entry's version list collapse state.
fn saved_versions_open_id(id: u64) -> egui::Id {
    egui::Id::new(("crusty_saved_versions_open", id))
}

/// Whether one saved entry's version list renders expanded. Defaults to
/// collapsed, so rows stay one line until asked.
fn saved_versions_open(ctx: &egui::Context, id: u64) -> bool {
    ctx.data_mut(|d| d.get_temp(saved_versions_open_id(id))).unwrap_or(false)
}

/// Flip one saved entry's version list collapse state.
fn set_saved_versions_open(ctx: &egui::Context, id: u64, open: bool) {
    ctx.data_mut(|d| d.insert_temp(saved_versions_open_id(id), open));
}

/// One version row label: its recency rank (v1 is newest) plus the truncated
/// first line, so identical texts still tell apart by rank.
fn saved_version_label(index: usize, version: &SavedQueryVersion) -> String {
    format!("v{} · {}", index + 1, history_title(&version.sql))
}

/// The sidebar's restore gesture: restore one version's text into its saved
/// entry, then open the entry so the restored text lands in a tab. True only
/// when the version existed. Preview uses `open_saved_version` directly.
pub(crate) fn restore_version_then_open(state: &mut AppState, id: u64, index: usize) -> bool {
    if state.restore_saved_version(id, index) {
        state.open_saved(id);
        true
    } else {
        false
    }
}

/// Whether a modal owns the keyboard, mirroring the editor's gate, so the
/// version Restore and Preview buttons stay disabled under one.
fn sidebar_modal_open(state: &AppState) -> bool {
    state.draft().is_some()
        || state.save_prompt().is_some()
        || state.password_prompt().is_some()
        || state.import_draft().is_some()
        || state.pending_write().is_some()
        || state.pending_params().is_some()
        || state.pending_edit_preview().is_some()
        || state.table_editor().is_some()
        || state.pending_table_ddl().is_some()
        || state.palette_open()
}

/// One saved-query row: pin mark, open button, connection name, delete, plus
/// a collapsible version list. The list renders `versions` newest first with
/// one Restore button (restore into the entry, then open it) and one Preview
/// button (open the old text without touching the entry) per row. Clicks
/// collect into `actions` like every other saved gesture, applied after the
/// borrow ends; both buttons stay disabled while a modal is open.
fn saved_row(
    ui: &mut egui::Ui,
    palette: &Palette,
    query: &SavedQuery,
    versions: &[SavedQueryVersion],
    modal_open: bool,
    conn_names: &HashMap<ConnectionId, String>,
    actions: &mut Vec<SidebarAction>,
) {
    ui.vertical(|ui| {
        ui.horizontal(|ui| {
            if query.pinned {
                ui.label(egui::RichText::new("★").small().color(palette.accent));
            }
            let name = ui
                .dnd_drag_source(
                    egui::Id::new(("crusty_saved_drag", query.id)),
                    SavedDrag { id: query.id },
                    |ui| ui.small_button(&query.name),
                )
                .inner;
            let name = name.on_hover_text("drag onto a folder to move");
            if name.clicked() {
                actions.push(SidebarAction::OpenSaved(query.id));
            }
            let conn_name = conn_names.get(&query.conn).map(String::as_str).unwrap_or("deleted");
            ui.label(egui::RichText::new(conn_name).small().weak());
            if ui.small_button("Delete").clicked() {
                actions.push(SidebarAction::DeleteSaved(query.id));
            }
            if !versions.is_empty() {
                let open = saved_versions_open(ui.ctx(), query.id);
                let arrow = if open { "\u{23F7}" } else { "\u{23F5}" };
                if ui
                    .small_button(format!("{arrow} History ({})", versions.len()))
                    .on_hover_text("Show per-save versions, newest first")
                    .clicked()
                {
                    set_saved_versions_open(ui.ctx(), query.id, !open);
                }
            }
        });
        if saved_versions_open(ui.ctx(), query.id) && !versions.is_empty() {
            indented(ui, 1, |ui| {
                for (index, version) in versions.iter().enumerate() {
                    ui.horizontal(|ui| {
                        ui.label(
                            egui::RichText::new(saved_version_label(index, version)).small().weak(),
                        )
                        .on_hover_text(version.sql.clone());
                        if ui
                            .add_enabled(
                                !modal_open,
                                egui::Button::new("Restore").small(),
                            )
                            .on_hover_text("Restore this text into the entry and open it")
                            .clicked()
                        {
                            actions.push(SidebarAction::RestoreSavedVersion(query.id, index));
                        }
                        if ui
                            .add_enabled(!modal_open, egui::Button::new("Preview").small())
                            .on_hover_text("Open this text in a tab without changing the entry")
                            .clicked()
                        {
                            actions.push(SidebarAction::OpenSavedVersion(query.id, index));
                        }
                    });
                }
            });
        }
    });
}

/// One connection card: status dot, name, environment, state, buttons, and
/// the live schema tree. The status dot is the drag source: dragging it
/// onto a folder header moves the card there through
/// [`SidebarAction::MoveConnection`].
#[allow(clippy::too_many_arguments)]
fn connection_card(
    state: &mut AppState,
    ui: &mut egui::Ui,
    palette: &Palette,
    conn_id: ConnectionId,
    conn_name: &str,
    env: Environment,
    status: ConnectionStatus,
    state_text: &str,
    selected: bool,
    actions: &mut Vec<SidebarAction>,
) {
    let frame_fill =
        if selected { palette.selected_card_fill } else { ui.visuals().faint_bg_color };
    let frame_stroke = if selected {
        egui::Stroke::new(1.0, palette.accent)
    } else {
        ui.visuals().widgets.noninteractive.bg_stroke
    };
    let transaction_open = state
        .connections()
        .iter()
        .find(|c| c.id == conn_id)
        .is_some_and(|c| c.transaction().is_open());
    egui::Frame::group(ui.style())
        .fill(frame_fill)
        .stroke(frame_stroke)
        .corner_radius(egui::CornerRadius::same(6))
        .inner_margin(egui::Margin::same(8))
        .show(ui, |ui| {
            ui.set_min_width(ui.available_width() - 16.0);
            ui.horizontal(|ui| {
                let dot_size = ui.text_style_height(&egui::TextStyle::Body);
                let _ = ui
                    .dnd_drag_source(
                        egui::Id::new(("crusty_conn_drag", conn_id.raw())),
                        ConnDrag { id: conn_id },
                        |ui| state_dot(ui, status, dot_size),
                    )
                    .inner
                    .on_hover_text("drag onto a folder to move");
                ui.vertical(|ui| {
                    ui.horizontal(|ui| {
                        ui.label(egui::RichText::new(conn_name).strong());
                        env_badge(ui, env);
                    });
                    ui.label(egui::RichText::new(state_text).small().weak());
                    if transaction_open {
                        ui.label(
                            egui::RichText::new("transaction open")
                                .small()
                                .color(palette.success),
                        );
                    }
                });
            });
            ui.add_space(4.0);
            ui.horizontal(|ui| {
                match status {
                    ConnectionStatus::Connected => {
                        if ui
                            .add(
                                egui::Button::new(
                                    egui::RichText::new("New query")
                                        .small()
                                        .strong(),
                                )
                                .fill(palette.accent)
                                .small(),
                            )
                            .clicked()
                        {
                            actions.push(SidebarAction::NewQuery(conn_id));
                        }
                        if ui.small_button("Disconnect").clicked() {
                            actions.push(SidebarAction::Disconnect(conn_id));
                        }
                    }
                    ConnectionStatus::Connecting => {
                        if ui.small_button("Stop").clicked() {
                            actions.push(SidebarAction::Disconnect(conn_id));
                        }
                    }
                    ConnectionStatus::Failed | ConnectionStatus::Offline => {
                        if ui.small_button("Connect").clicked() {
                            actions.push(SidebarAction::Connect(conn_id));
                        }
                    }
                }
                if ui.small_button("Edit").clicked() {
                    actions.push(SidebarAction::Edit(conn_id));
                }
                if ui.small_button("Delete").clicked() {
                    actions.push(SidebarAction::Delete(conn_id));
                }
            });
            if matches!(status, ConnectionStatus::Connected) {
                ui.add_space(4.0);
                ui.separator();
                ui.add_space(4.0);
                let schema_actions = super::schema::connection_schema(
                    state, ui, conn_id,
                );
                for sa in schema_actions {
                    match sa {
                        super::schema::SchemaAction::Refresh(id) => {
                            actions.push(SidebarAction::RefreshSchema(id));
                        }
                        super::schema::SchemaAction::Preview(id, schema, table) => {
                            actions.push(SidebarAction::PreviewTable(
                                id, schema, table,
                            ));
                        }
                        super::schema::SchemaAction::Reveal(id, schema, table) => {
                            actions.push(SidebarAction::RevealTable(
                                id, schema, table,
                            ));
                        }
                        super::schema::SchemaAction::Count(id, schema, table) => {
                            actions.push(SidebarAction::TableCount(
                                id, schema, table,
                            ));
                        }
                        super::schema::SchemaAction::Bookmark(id, schema, table) => {
                            actions.push(SidebarAction::ToggleBookmark(
                                id, schema, table,
                            ));
                        }
                        super::schema::SchemaAction::Sessions(id) => {
                            actions.push(SidebarAction::RefreshSessions(id));
                        }
                        super::schema::SchemaAction::Terminate(id, backend) => {
                            actions.push(SidebarAction::TerminateSession(id, backend));
                        }
                        super::schema::SchemaAction::RevealHandled => {
                            state.clear_navigator_reveal();
                        }
                    }
                }
            }
        });
}

/// The inline editor for one armed folder creation input: a name box with
/// Create and Cancel. Enter while the box holds focus creates too.
fn folder_creation_editor(ui: &mut egui::Ui, scope: &str, parent: &str) {
    ui.horizontal(|ui| {
        let mut text = folder_input(ui.ctx(), scope, parent, "text");
        let editor = ui.add(
            egui::TextEdit::singleline(&mut text)
                .hint_text("folder name")
                .desired_width(120.0),
        );
        if text.is_empty() {
            editor.request_focus();
        }
        set_folder_input(ui.ctx(), scope, parent, "text", text);
        let mut confirm = ui.small_button("Create").clicked();
        if editor.has_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter)) {
            confirm = true;
        }
        if confirm {
            confirm_folder_input(ui.ctx(), scope, parent);
        }
        if ui.small_button("Cancel").clicked() {
            disarm_folder_input(ui.ctx(), scope, parent);
        }
    });
}

/// Paint the drop highlight on a folder row while a matching payload hovers.
fn paint_drop_highlight(ui: &mut egui::Ui, palette: &Palette, rect: egui::Rect) {
    ui.painter().rect_stroke(
        rect.expand(2.0),
        egui::CornerRadius::same(4),
        egui::Stroke::new(1.5, palette.accent),
        egui::StrokeKind::Middle,
    );
}

/// One connection folder header: collapse arrow, name, entry count, new
/// subfolder and new-connection-here affordances. The row is the drop zone
/// for its path; a matching release moves the card there.
#[allow(clippy::too_many_arguments)]
fn conn_folder_header(
    ui: &mut egui::Ui,
    palette: &Palette,
    path: &str,
    name: &str,
    depth: usize,
    open: bool,
    count: usize,
    actions: &mut Vec<SidebarAction>,
) {
    indented(ui, depth, |ui| {
        let resp = ui
            .horizontal(|ui| {
                let arrow = if open { "\u{23F7}" } else { "\u{23F5}" };
                if ui.small_button(format!("{arrow} {name}")).clicked() {
                    set_folder_open(ui.ctx(), "conn", path, !open);
                }
                ui.label(egui::RichText::new(format!("{count}")).small().weak());
                if ui.small_button("+").on_hover_text("New subfolder").clicked() {
                    arm_folder_input(ui.ctx(), "conn", path);
                }
                if ui
                    .small_button("+conn")
                    .on_hover_text("New connection in this folder")
                    .clicked()
                {
                    actions.push(SidebarAction::NewConnectionInFolder(path.to_owned()));
                }
            })
            .response;
        if resp.dnd_hover_payload::<ConnDrag>().is_some() {
            paint_drop_highlight(ui, palette, resp.rect);
        }
        if let Some(payload) = resp.dnd_release_payload::<ConnDrag>() {
            actions.push(SidebarAction::MoveConnection(payload.id, Some(path.to_owned())));
        }
        if folder_input_armed(ui.ctx(), "conn", path) {
            folder_creation_editor(ui, "conn", path);
        }
    });
}

/// One saved-query folder header: collapse arrow, name, entry count, and a
/// new subfolder affordance. The row is the drop zone for its path.
#[allow(clippy::too_many_arguments)]
fn saved_folder_header(
    ui: &mut egui::Ui,
    palette: &Palette,
    path: &str,
    name: &str,
    depth: usize,
    open: bool,
    count: usize,
    actions: &mut Vec<SidebarAction>,
) {
    indented(ui, depth, |ui| {
        let resp = ui
            .horizontal(|ui| {
                let arrow = if open { "\u{23F7}" } else { "\u{23F5}" };
                if ui.small_button(format!("{arrow} {name}")).clicked() {
                    set_folder_open(ui.ctx(), "saved", path, !open);
                }
                ui.label(egui::RichText::new(format!("{count}")).small().weak());
                if ui.small_button("+").on_hover_text("New subfolder").clicked() {
                    arm_folder_input(ui.ctx(), "saved", path);
                }
            })
            .response;
        if resp.dnd_hover_payload::<SavedDrag>().is_some() {
            paint_drop_highlight(ui, palette, resp.rect);
        }
        if let Some(payload) = resp.dnd_release_payload::<SavedDrag>() {
            actions.push(SidebarAction::MoveSaved(payload.id, Some(path.to_owned())));
        }
        if folder_input_armed(ui.ctx(), "saved", path) {
            folder_creation_editor(ui, "saved", path);
        }
    });
}

pub fn sidebar(state: &mut AppState, ui: &mut egui::Ui) {
    // root() pumps this module through the sidebar, so the prod confirm
    // renders with the connect gestures that raise it.
    prod_confirm_window(state, ui);
    terminate_confirm_window(state, ui);
    let palette = Palette::of(ui.ctx().theme());
    ui.with_layout(egui::Layout::bottom_up(egui::Align::Min), |ui| {
        if ui
            .add_sized(
                [ui.available_width(), 28.0],
                egui::Button::new(egui::RichText::new("+ New connection").strong()).fill(palette.accent),
            )
            .clicked()
        {
            state.open_draft(None);
        }
        ui.separator();
        egui::ScrollArea::vertical().show(ui, |ui| {
            ui.with_layout(egui::Layout::top_down(egui::Align::LEFT), |ui| {
                let mut actions: Vec<SidebarAction> = Vec::new();
                let no_connections = state.connections().is_empty();
                let conn_header = ui
                    .horizontal(|ui| {
                        ui.label(egui::RichText::new("CONNECTIONS").small().weak());
                        if !no_connections
                            && ui.small_button("+ Folder").on_hover_text("New folder").clicked()
                        {
                            arm_folder_input(ui.ctx(), "conn", "");
                        }
                        if egui::DragAndDrop::has_payload_of_type::<ConnDrag>(ui.ctx()) {
                            ui.label(
                                egui::RichText::new("drop here for root").small().weak(),
                            );
                        }
                    })
                    .response;
                if conn_header.dnd_hover_payload::<ConnDrag>().is_some() {
                    paint_drop_highlight(ui, &palette, conn_header.rect);
                }
                if let Some(payload) = conn_header.dnd_release_payload::<ConnDrag>() {
                    actions.push(SidebarAction::MoveConnection(payload.id, None));
                }
                if folder_input_armed(ui.ctx(), "conn", "") {
                    folder_creation_editor(ui, "conn", "");
                }
                ui.add_space(4.0);
                ui.horizontal(|ui| {
                    for view in SidebarView::ALL {
                        if ui
                            .selectable_label(state.sidebar_view() == view, view.label())
                            .clicked()
                        {
                            state.set_sidebar_view(view);
                        }
                    }
                });
                ui.add_space(4.0);

                // The saved rows and the action loop both gate on a modal, so
                // the flag lives outside the section gate.
                let modal_open = sidebar_modal_open(state);
                match state.sidebar_view() {
                    SidebarView::Schema => {
                        if !no_connections {
                            ui.horizontal(|ui| {
                                ui.label(egui::RichText::new("Filter tables:").small().weak());
                                let mut filter = state.navigator_filter().to_owned();
                                if ui
                                    .add(
                                        egui::TextEdit::singleline(&mut filter)
                                            .hint_text("narrow tables by name")
                                            .desired_width(120.0),
                                    )
                                    .changed()
                                {
                                    state.set_navigator_filter(filter);
                                }
                            });
                            ui.add_space(4.0);
                        }

                        if no_connections {
                            ui.label("No connections yet.");
                            ui.small("Add one below to start querying.");
                            ui.add_space(8.0);
                        }

                        let active_conn = state
                            .active()
                            .and_then(|id| state.tab(id))
                            .and_then(|tab| tab.conn);

                        let snapshot: Vec<CardEntry> = state
                            .connections()
                            .iter()
                            .map(|c| {
                                let state_text = match c.state() {
                                    ConnectionState::Disconnected => "disconnected".to_owned(),
                                    ConnectionState::Connecting { .. } => "connecting".to_owned(),
                                    ConnectionState::Connected { session } => {
                                        session.server_version.clone()
                                    }
                                    ConnectionState::QueryRunning { session, .. } => {
                                        session.server_version.clone()
                                    }
                                    ConnectionState::Error { message } => message.clone(),
                                };
                                CardEntry {
                                    id: c.id,
                                    name: c.name.clone(),
                                    env: c.env,
                                    status: ConnectionStatus::of(c.state()),
                                    state_text,
                                }
                            })
                            .collect();
                        let conn_plan: Vec<ConnPlan> = {
                            let mut tree = connection_folder_tree(state.connections());
                            let mut extra: Vec<String> =
                                created_folders(ui.ctx(), "conn").into_iter().collect();
                            extra.sort();
                            for path in &extra {
                                tree.ensure_path(path);
                            }
                            let mut plan = Vec::new();
                            for entry in &tree.entries {
                                plan.push(ConnPlan::Card { id: entry.id, depth: 0 });
                            }
                            flatten_connections(ui.ctx(), &tree, 0, &mut plan);
                            plan
                        };
                        for item in &conn_plan {
                            let (conn_id, depth) = match item {
                                ConnPlan::Folder { path, name, depth, open, count } => {
                                    conn_folder_header(
                                        ui, &palette, path, name, *depth, *open, *count, &mut actions,
                                    );
                                    continue;
                                }
                                ConnPlan::Card { id, depth } => (*id, *depth),
                            };
                            let Some(card) = snapshot.iter().find(|card| card.id == conn_id)
                            else {
                                continue;
                            };
                            let selected = Some(conn_id) == active_conn;
                            indented(ui, depth, |ui| {
                                connection_card(
                                    state,
                                    ui,
                                    &palette,
                                    conn_id,
                                    &card.name,
                                    card.env,
                                    card.status,
                                    &card.state_text,
                                    selected,
                                    &mut actions,
                                );
                            });
                            ui.add_space(4.0);
                        }

                        if !no_connections || !state.bookmarks().is_empty() {
                            ui.label(egui::RichText::new("BOOKMARKS").small().weak());
                            ui.add_space(4.0);
                            if state.bookmarks().is_empty() {
                                ui.label(egui::RichText::new("No bookmarks.").small().weak());
                            } else {
                                let conn_names: HashMap<ConnectionId, String> = state
                                    .connections()
                                    .iter()
                                    .map(|c| (c.id, c.name.clone()))
                                    .collect();
                                for bookmark in state.bookmarks().to_vec() {
                                    ui.horizontal(|ui| {
                                        ui.label(egui::RichText::new("★").small().color(palette.accent));
                                        let title = format!("{}.{}", bookmark.schema, bookmark.table);
                                        if ui.small_button(&title).clicked() {
                                            actions.push(SidebarAction::PreviewTable(
                                                bookmark.conn,
                                                bookmark.schema.clone(),
                                                bookmark.table.clone(),
                                            ));
                                        }
                                        let conn_name = conn_names
                                            .get(&bookmark.conn)
                                            .map(String::as_str)
                                            .unwrap_or("deleted");
                                        ui.label(egui::RichText::new(conn_name).small().weak());
                                        if ui.small_button("Remove").clicked() {
                                            actions.push(SidebarAction::ToggleBookmark(
                                                bookmark.conn,
                                                bookmark.schema.clone(),
                                                bookmark.table.clone(),
                                            ));
                                        }
                                    });
                                }
                            }
                            ui.add_space(8.0);
                        }
                    }
                    SidebarView::History => {
                        // A chosen view always shows its section, even when
                        // there is nothing in it yet.
                        ui.horizontal(|ui| {
                            ui.label(egui::RichText::new("HISTORY").small().weak());
                            let scope_label =
                                if state.history_current_only() { "Current" } else { "All" };
                            if ui
                                .small_button(format!("Scope: {scope_label}"))
                                .on_hover_text(
                                    "Toggle the log between the current connection and all connections",
                                )
                                .clicked()
                            {
                                state.toggle_history_scope();
                            }
                            if ui.small_button("Clear").clicked() {
                                actions.push(SidebarAction::ClearHistory);
                            }
                        });
                        ui.add_space(4.0);
                        let history_conn_names: HashMap<ConnectionId, String> = state
                            .connections()
                            .iter()
                            .map(|c| (c.id, c.name.clone()))
                            .collect();
                        let scope_current_only = state.history_current_only();
                        let scope_conn = state.history_scope_connection();
                        let show_empty = state.history().is_empty();
                        if show_empty {
                            ui.label(egui::RichText::new("No runs yet.").small().weak());
                        } else if scope_current_only && scope_conn.is_none() {
                            ui.label(
                                egui::RichText::new("No current connection.")
                                    .small()
                                    .weak(),
                            );
                        } else {
                            let scoped: Vec<(ConnectionId, String, bool, Option<u64>, std::time::Duration)> =
                                match (scope_current_only, scope_conn) {
                                    (true, Some(conn)) => state
                                        .history_for(conn)
                                        .into_iter()
                                        .take(crate::app::HISTORY_SHOWN)
                                        .map(|entry| {
                                            (
                                                entry.conn,
                                                entry.sql.clone(),
                                                entry.succeeded,
                                                entry.rows,
                                                entry.took,
                                            )
                                        })
                                        .collect(),
                                    (true, None) => Vec::new(),
                                    (false, _) => state
                                        .history()
                                        .iter()
                                        .take(crate::app::HISTORY_SHOWN)
                                        .map(|entry| {
                                            (
                                                entry.conn,
                                                entry.sql.clone(),
                                                entry.succeeded,
                                                entry.rows,
                                                entry.took,
                                            )
                                        })
                                        .collect(),
                                };
                            if scoped.is_empty() {
                                ui.label(
                                    egui::RichText::new("No runs for this connection.")
                                        .small()
                                        .weak(),
                                );
                            } else {
                                for (conn, sql, succeeded, rows, took) in scoped {
                                    let dot =
                                        if succeeded { palette.success } else { egui::Color32::RED };
                                    let entry = crate::app::HistoryEntry {
                                        conn,
                                        sql: sql.clone(),
                                        succeeded,
                                        rows,
                                        took,
                                        at: 0,
                                    };
                                    let stats = crate::app::history_stats(&entry);
                                    let conn_name = history_conn_names
                                        .get(&conn)
                                        .map(String::as_str)
                                        .unwrap_or("deleted");
                                    let live = state.is_connection_live(conn);
                                    ui.horizontal(|ui| {
                                        filled_dot(
                                            ui,
                                            dot,
                                            ui.text_style_height(&egui::TextStyle::Body),
                                        );
                                        if ui.small_button(history_title(&sql)).clicked() {
                                            actions.push(SidebarAction::OpenHistory(
                                                conn,
                                                sql.clone(),
                                            ));
                                        }
                                        if live && ui.small_button("Rerun").clicked() {
                                            actions.push(SidebarAction::OpenHistory(conn, sql.clone()));
                                        }
                                    });
                                    ui.label(
                                        egui::RichText::new(format!("{stats} · {conn_name}"))
                                            .small()
                                            .weak(),
                                    );
                                }
                            }
                        }
                        ui.add_space(8.0);
                    }
                    SidebarView::Saved => {
                        let saved_header = ui
                            .horizontal(|ui| {
                                ui.label(egui::RichText::new("SAVED").small().weak());
                                if ui.small_button("+ Folder").on_hover_text("New folder").clicked() {
                                    arm_folder_input(ui.ctx(), "saved", "");
                                }
                                if egui::DragAndDrop::has_payload_of_type::<SavedDrag>(ui.ctx()) {
                                    ui.label(
                                        egui::RichText::new("drop here for root").small().weak(),
                                    );
                                }
                            })
                            .response;
                        if saved_header.dnd_hover_payload::<SavedDrag>().is_some() {
                            paint_drop_highlight(ui, &palette, saved_header.rect);
                        }
                        if let Some(payload) = saved_header.dnd_release_payload::<SavedDrag>() {
                            actions.push(SidebarAction::MoveSaved(payload.id, None));
                        }
                        if folder_input_armed(ui.ctx(), "saved", "") {
                            folder_creation_editor(ui, "saved", "");
                        }
                        ui.add_space(4.0);
                        let conn_names: HashMap<ConnectionId, String> = state
                            .connections()
                            .iter()
                            .map(|c| (c.id, c.name.clone()))
                            .collect();
                        let saved_versions: HashMap<u64, Vec<SavedQueryVersion>> = state
                            .saved_queries()
                            .iter()
                            .map(|q| (q.id, state.saved_versions(q.id)))
                            .collect();
                        let saved_plan: Vec<SavedPlan<'_>> = {
                            let mut tree = saved_folder_tree(state.saved_queries());
                            let mut extra: Vec<String> =
                                created_folders(ui.ctx(), "saved").into_iter().collect();
                            extra.sort();
                            for path in &extra {
                                tree.ensure_path(path);
                            }
                            let mut plan = Vec::new();
                            for query in &tree.entries {
                                plan.push(SavedPlan::Entry { query, depth: 0 });
                            }
                            flatten_saved(ui.ctx(), &tree, 0, &mut plan);
                            plan
                        };
                        if saved_plan.is_empty() {
                            ui.label(egui::RichText::new("No saved queries.").small().weak());
                        } else {
                            for item in &saved_plan {
                                match item {
                                    SavedPlan::Folder { path, name, depth, open, count } => {
                                        saved_folder_header(
                                            ui, &palette, path, name, *depth, *open, *count,
                                            &mut actions,
                                        );
                                    }
                                    SavedPlan::Entry { query, depth } => {
                                        indented(ui, *depth, |ui| {
                                            let empty: Vec<SavedQueryVersion> = Vec::new();
                                            let versions = saved_versions
                                                .get(&query.id)
                                                .unwrap_or(&empty);
                                            saved_row(
                                                ui,
                                                &palette,
                                                query,
                                                versions,
                                                modal_open,
                                                &conn_names,
                                                &mut actions,
                                            );
                                        });
                                    }
                                }
                            }
                        }
                        ui.add_space(8.0);
                        ui.horizontal(|ui| {
                            ui.label(egui::RichText::new("Import SQL:").small().weak());
                            let mut folder = sql_import_folder(ui.ctx());
                            if ui
                                .add(
                                    egui::TextEdit::singleline(&mut folder)
                                        .hint_text("folder")
                                        .desired_width(100.0),
                                )
                                .changed()
                            {
                                set_sql_import_folder(ui.ctx(), folder.clone());
                            }
                            if ui
                                .small_button("Files")
                                .on_hover_text("Pick .sql files, one saved query per file")
                                .clicked()
                            {
                                import_picked_sql_files(&mut *state, ui.ctx(), &folder);
                            }
                            if ui
                                .small_button("Dir")
                                .on_hover_text("Import every .sql file in a directory")
                                .clicked()
                            {
                                import_picked_sql_dir(&mut *state, ui.ctx(), &folder);
                            }
                        });
                        if let Some(note) = sql_import_note_of(ui.ctx()) {
                            ui.label(egui::RichText::new(note).small().weak());
                        }
                        ui.add_space(8.0);
                    }
                }

                for action in actions {
                    match action {
                        SidebarAction::Connect(id) => {
                            let needs_prompt = state
                                .connections()
                                .iter()
                                .find(|c| c.id == id)
                                .is_some_and(|c| {
                                    c.config.prompts_for_password() && c.password.is_empty()
                                });
                            if needs_prompt {
                                state.open_password_prompt(id);
                            } else {
                                state.connect(id);
                            }
                        }
                        SidebarAction::Disconnect(id) => state.disconnect(id),
                        SidebarAction::NewQuery(id) => {
                            let tab = state.new_tab(id);
                            state.activate(tab);
                        }
                        SidebarAction::Edit(id) => state.open_draft(Some(id)),
                        SidebarAction::Delete(id) => state.delete_connection(id),
                        SidebarAction::RefreshSchema(id) => state.refresh_schema(id),
                        SidebarAction::PreviewTable(id, schema, table) => {
                            let _ = state.open_table_view(id, &schema, &table);
                        }
                        SidebarAction::RevealTable(id, schema, table) => {
                            state.reveal_table(id, &schema, &table)
                        }
                        SidebarAction::TableCount(id, schema, table) => {
                            state.request_row_count(id, &schema, &table)
                        }
                        SidebarAction::ToggleBookmark(id, schema, table) => {
                            state.toggle_bookmark(id, &schema, &table);
                        }
                        SidebarAction::RefreshSessions(id) => {
                            state.request_sessions(id);
                        }
                        SidebarAction::TerminateSession(id, backend) => {
                            state.request_terminate(id, &backend);
                        }
                        SidebarAction::OpenHistory(conn, sql) => state.open_history(conn, &sql),
                        SidebarAction::ClearHistory => state.clear_history(),
                        SidebarAction::OpenSaved(id) => state.open_saved(id),
                        SidebarAction::DeleteSaved(id) => {
                            state.delete_saved(id);
                        }
                        SidebarAction::RestoreSavedVersion(id, index) => {
                            if !modal_open {
                                restore_version_then_open(state, id, index);
                            }
                        }
                        SidebarAction::OpenSavedVersion(id, index) => {
                            if !modal_open {
                                state.open_saved_version(id, index);
                            }
                        }
                        SidebarAction::MoveSaved(id, folder) => {
                            state.move_saved(id, folder.as_deref());
                        }
                        SidebarAction::MoveConnection(id, folder) => {
                            state.move_connection(id, folder.as_deref());
                        }
                        SidebarAction::NewConnectionInFolder(path) => {
                            state.open_draft(None);
                            if let Some(draft) = state.draft_mut() {
                                draft.folder = path;
                            }
                        }
                    }
                }
            });
        });
    });
}

/// A pasted connection JSON blob split into its persisted shape, its secret,
/// and its display name. The same triple the URL paste fills, so the JSON
/// row lands through the same draft assignment.
#[derive(Debug, Clone, PartialEq)]
pub struct ParsedConnectionJson {
    pub kind: DbKind,
    pub config: ConnectionConfig,
    pub password: Option<String>,
    pub name: Option<String>,
}

/// Pure parser for a Beekeeper-style connection JSON blob: `connectionType`
/// (`postgresql`, `postgres`, `psql`, `mysql`, `mariadb`, `sqlite`), `host`,
/// `port`, `username`, `password`, and `defaultDatabase`, with `user`,
/// `database`, and `name` accepted as hand-written aliases. A `url` (or
/// `connectionString`) field delegates to [`parse_connection_url`] whole,
/// keeping one URL grammar. `ssl: true` maps onto `Require` (Postgres) or
/// `Required` (MySQL); a missing flag keeps the kind default, so old blobs
/// behave like today's form. Any malformed input reports one
/// [`DbError::Config`] message.
pub fn parse_connection_json(raw: &str) -> Result<ParsedConnectionJson, DbError> {
    let value: serde_json::Value =
        serde_json::from_str(raw.trim()).map_err(|_| invalid_json())?;
    let object = value.as_object().ok_or_else(invalid_json)?;
    if let Some(url) = json_text(object, &["url", "connectionString"]) {
        let parsed = parse_connection_url(&url)?;
        return Ok(ParsedConnectionJson {
            kind: parsed.kind,
            config: parsed.config,
            password: parsed.password,
            name: json_text(object, &["name", "label"]),
        });
    }
    let type_raw = json_text(object, &["connectionType", "type", "kind"])
        .ok_or_else(invalid_json)?;
    let kind = match type_raw.to_ascii_lowercase().as_str() {
        "postgresql" | "postgres" | "psql" => DbKind::Postgres,
        "mysql" | "mariadb" => DbKind::Mysql,
        "sqlite" => DbKind::Sqlite,
        _ => return Err(unsupported_type(&type_raw)),
    };
    let password = json_text(object, &["password"]);
    let name = json_text(object, &["name", "label"]);
    let read_only = json_flag(object, &["readOnlyMode", "readOnly", "read_only"]);
    let config = match kind {
        DbKind::Postgres | DbKind::Mysql => {
            let ssl = json_flag(object, &["ssl"]);
            let host =
                json_text(object, &["host"]).unwrap_or_else(|| "localhost".to_owned());
            let port = json_port(object)?.unwrap_or_else(|| {
                kind.default_port().expect("network kinds carry a default port")
            });
            let database =
                json_text(object, &["defaultDatabase", "database", "db"]).unwrap_or_default();
            let user = json_text(object, &["username", "user"]).unwrap_or_default();
            let mut config = match kind {
                DbKind::Postgres => ConnectionConfig::Postgres {
                    host,
                    port,
                    database,
                    user,
                    ssl_mode: if ssl {
                        PostgresSslMode::Require
                    } else {
                        PostgresSslMode::Disable
                    },
                    ssh: None,
                    read_only: false,
                },
                _ => ConnectionConfig::Mysql {
                    host,
                    port,
                    database,
                    user,
                    ssl_mode: if ssl {
                        MysqlSslMode::Required
                    } else {
                        MysqlSslMode::default()
                    },
                    ssh: None,
                    read_only: false,
                },
            };
            if read_only {
                config.set_read_only(true);
            }
            config
        }
        DbKind::Sqlite => {
            let path = json_text(object, &["defaultDatabase", "database", "path", "filename"])
                .ok_or_else(invalid_json)?;
            let mut config =
                ConnectionConfig::Sqlite { path: path.into(), read_only: false };
            if read_only {
                config.set_read_only(true);
            }
            config
        }
    };
    Ok(ParsedConnectionJson { kind, config, password, name })
}

fn invalid_json() -> DbError {
    DbError::Config("invalid connection JSON".to_owned())
}

fn unsupported_type(raw: &str) -> DbError {
    DbError::Config(format!("unsupported connection type {raw:?}"))
}

/// The first non-blank string under any of `keys`, trimmed. Missing, null,
/// non-string, and blank values all read as absent, so partial blobs fall
/// back to the kind defaults.
fn json_text(
    object: &serde_json::Map<String, serde_json::Value>,
    keys: &[&str],
) -> Option<String> {
    keys.iter().find_map(|key| match object.get(*key) {
        Some(serde_json::Value::String(text)) => {
            let trimmed = text.trim();
            (!trimmed.is_empty()).then(|| trimmed.to_owned())
        }
        _ => None,
    })
}

/// The port under `port`: a number or a numeric string. Missing or null
/// reads as absent for the kind default; anything else malformed errors.
fn json_port(
    object: &serde_json::Map<String, serde_json::Value>,
) -> Result<Option<u16>, DbError> {
    let value = object.get("port").unwrap_or(&serde_json::Value::Null);
    match value {
        serde_json::Value::Null => Ok(None),
        serde_json::Value::Number(n) => {
            n.as_u64().and_then(|n| u16::try_from(n).ok()).filter(|n| *n != 0)
                .map(Some)
                .ok_or_else(invalid_json)
        }
        serde_json::Value::String(text) => {
            text.trim().parse::<u16>().ok().filter(|n| *n != 0).map(Some)
                .ok_or_else(invalid_json)
        }
        _ => Err(invalid_json()),
    }
}

/// Whether any of `keys` carries boolean true. Missing and non-boolean
/// values read as false, so blobs without the flag keep today's behavior.
fn json_flag(object: &serde_json::Map<String, serde_json::Value>, keys: &[&str]) -> bool {
    keys.iter().any(|key| object.get(*key).is_some_and(|v| v.as_bool().unwrap_or(false)))
}

pub fn password_window(state: &mut AppState, ui: &mut egui::Ui) {
    let Some(id) = state.password_prompt() else { return };
    let Some(name) =
        state.connections().iter().find(|c| c.id == id).map(|c| c.name.clone())
    else {
        state.cancel_password_prompt();
        return;
    };
    let mut submit = false;
    let mut cancel = false;
    egui::Window::new(format!("Password for {name}")).show(ui.ctx(), |ui| {
        ui.label(format!("Enter password for {name}"));
        ui.add(egui::TextEdit::singleline(state.prompt_password_mut()).password(true));
        ui.horizontal(|ui| {
            if ui.button("Connect").clicked() {
                submit = true;
            }
            if ui.button("Cancel").clicked() {
                cancel = true;
            }
        });
    });
    if submit {
        state.submit_password();
    } else if cancel {
        state.cancel_password_prompt();
    }
}

pub fn prod_confirm_window(state: &mut AppState, ui: &mut egui::Ui) {
    let Some(id) = state.pending_prod() else { return };
    let Some(name) =
        state.connections().iter().find(|c| c.id == id).map(|c| c.name.clone())
    else {
        state.cancel_prod();
        return;
    };
    let mut confirm = false;
    let mut cancel = false;
    egui::Window::new("Connect to prod?").show(ui.ctx(), |ui| {
        ui.label(format!("Connect to {name}?"));
        ui.horizontal(|ui| {
            if ui.button("Connect").clicked() {
                confirm = true;
            }
            if ui.button("Cancel").clicked() {
                cancel = true;
            }
        });
    });
    if confirm {
        state.confirm_prod();
    } else if cancel {
        state.cancel_prod();
    }
}

/// The prod-gated terminate confirmation: names the backend and the
/// connection, with the production warning the connect confirm carries.
/// Non-prod terminates never arm this modal; they send on click.
pub fn terminate_confirm_window(state: &mut AppState, ui: &mut egui::Ui) {
    let Some(pending) = state.pending_terminate().cloned() else { return };
    let Some(name) =
        state.connections().iter().find(|c| c.id == pending.conn).map(|c| c.name.clone())
    else {
        state.cancel_terminate();
        return;
    };
    let mut confirm = false;
    let mut cancel = false;
    egui::Window::new("Terminate session?").show(ui.ctx(), |ui| {
        ui.label(format!("Terminate backend {} on {name}?", pending.backend));
        ui.label("The backend's running query stops and its connection drops.");
        if ui.button("Terminate").clicked() {
            confirm = true;
        }
        if ui.button("Cancel").clicked() {
            cancel = true;
        }
    });
    if confirm {
        state.confirm_terminate();
    } else if cancel {
        state.cancel_terminate();
    }
}

pub fn draft_window(state: &mut AppState, ui: &mut egui::Ui) {
    if state.draft().is_none() {
        return;
    }
    let title = if state.draft().is_some_and(|d| d.editing.is_some()) {
        "Edit connection"
    } else {
        "New connection"
    };
    let mut save = false;
    let mut close = false;
    egui::Window::new(title).show(ui.ctx(), |ui| {
        let registry = state.registry();
        let Some(draft) = state.draft_mut() else { return };
        let draft_scope = draft.editing.map(|id| id.raw());
        let url_id = egui::Id::new(("connection_url_paste", draft_scope));
        let note_id = egui::Id::new(("connection_url_note", draft_scope));
        let json_id = egui::Id::new(("connection_json_paste", draft_scope));
        let mut url_text: String =
            ui.ctx().memory(|m| m.data.get_temp(url_id)).unwrap_or_default();
        let mut json_text: String =
            ui.ctx().memory(|m| m.data.get_temp(json_id)).unwrap_or_default();
        let mut parse_url = false;
        let mut parse_json = false;
        ui.horizontal(|ui| {
            ui.label("URL:");
            ui.add(
                egui::TextEdit::singleline(&mut url_text)
                    .hint_text("postgres://user:pass@host:5432/db"),
            );
            parse_url = ui.button("Parse").clicked();
        });
        ui.ctx().memory_mut(|m| m.data.insert_temp(url_id, url_text.clone()));
        ui.horizontal(|ui| {
            ui.label("JSON:");
            ui.add(
                egui::TextEdit::singleline(&mut json_text)
                    .hint_text("{\"connectionType\":\"postgresql\", ...}"),
            );
            parse_json = ui
                .button("Parse")
                .on_hover_text("Fill the form from a Beekeeper-style connection JSON blob")
                .clicked();
        });
        ui.ctx().memory_mut(|m| m.data.insert_temp(json_id, json_text.clone()));
        if parse_url {
            match parse_connection_url(&url_text) {
                Ok(parsed) => {
                    let kind = parsed.kind;
                    draft.config = parsed.config;
                    if let Some(password) = parsed.password {
                        draft.password = password;
                    }
                    ui.ctx().memory_mut(|m| {
                        m.data.insert_temp(
                            note_id,
                            Some((
                                format!("Filled {} fields from URL.", kind.label()),
                                false,
                            )),
                        )
                    });
                }
                Err(error) => {
                    ui.ctx().memory_mut(|m| {
                        m.data.insert_temp(note_id, Some((error.to_string(), true)))
                    });
                }
            }
        }
        if parse_json {
            match parse_connection_json(&json_text) {
                Ok(parsed) => {
                    let kind = parsed.kind;
                    draft.config = parsed.config;
                    if let Some(password) = parsed.password {
                        draft.password = password;
                    }
                    if let Some(name) = parsed.name
                        && draft.name.trim().is_empty()
                    {
                        draft.name = name;
                    }
                    ui.ctx().memory_mut(|m| {
                        m.data.insert_temp(
                            note_id,
                            Some((
                                format!("Filled {} fields from JSON.", kind.label()),
                                false,
                            )),
                        )
                    });
                }
                Err(error) => {
                    ui.ctx().memory_mut(|m| {
                        m.data.insert_temp(note_id, Some((error.to_string(), true)))
                    });
                }
            }
        }
        if let Some((note, is_error)) = ui
            .ctx()
            .memory(|m| m.data.get_temp::<Option<(String, bool)>>(note_id))
            .flatten()
        {
            if is_error {
                ui.colored_label(ui.visuals().error_fg_color, note);
            } else {
                ui.label(egui::RichText::new(note).small().weak());
            }
        }
        ui.horizontal(|ui| {
            ui.label("Name:");
            ui.text_edit_singleline(&mut draft.name);
        });
        ui.horizontal(|ui| {
            ui.label("Folder:");
            ui.add(
                egui::TextEdit::singleline(&mut draft.folder)
                    .hint_text("optional nested path like ops/db"),
            );
        });
        ui.horizontal(|ui| {
            ui.label("Kind:");
            let mut selected = draft.config.kind();
            for kind in registry.all() {
                if ui.selectable_value(&mut selected, kind, kind.label()).clicked() {
                    draft.config = kind.default_config();
                }
            }
        });
        ui.horizontal(|ui| {
            ui.label("Environment:");
            for env in Environment::ALL {
                ui.selectable_value(&mut draft.env, env, env.label());
            }
        });
        let mut read_only = draft.config.is_read_only();
        if ui
            .checkbox(&mut read_only, "Read only")
            .on_hover_text("Refuse statements that write when set")
            .changed()
        {
            draft.config.set_read_only(read_only);
        }
        match &mut draft.config {
            ConnectionConfig::Sqlite { path, .. } => {
                ui.horizontal(|ui| {
                    ui.label("Path:");
                    let mut text = path.to_string_lossy().into_owned();
                    if ui.text_edit_singleline(&mut text).changed() {
                        *path = text.into();
                    }
                });
                ui.small("Use :memory: for a throwaway database.");
            }
            ConnectionConfig::Postgres { host, port, database, user, ssl_mode, ssh, .. } => {
                ui.horizontal(|ui| {
                    ui.label("Host:");
                    ui.text_edit_singleline(host);
                });
                ui.horizontal(|ui| {
                    ui.label("Port:");
                    let mut text = port.to_string();
                    if ui.text_edit_singleline(&mut text).changed() {
                        if let Ok(p) = text.parse() {
                            *port = p;
                        }
                    }
                });
                ui.horizontal(|ui| {
                    ui.label("Database:");
                    ui.text_edit_singleline(database);
                });
                ui.horizontal(|ui| {
                    ui.label("User:");
                    ui.text_edit_singleline(user);
                });
                ui.horizontal(|ui| {
                    ui.label("SSL:");
                    egui::ComboBox::from_label("")
                        .selected_text(ssl_mode.label())
                        .show_ui(ui, |ui| {
                            for mode in PostgresSslMode::ALL {
                                ui.selectable_value(ssl_mode, mode, mode.label());
                            }
                        });
                });
                ssh_section(ui, ssh, &mut draft.ssh_password);
            }
            ConnectionConfig::Mysql { host, port, database, user, ssl_mode, ssh, .. } => {
                ui.horizontal(|ui| {
                    ui.label("Host:");
                    ui.text_edit_singleline(host);
                });
                ui.horizontal(|ui| {
                    ui.label("Port:");
                    let mut text = port.to_string();
                    if ui.text_edit_singleline(&mut text).changed() {
                        if let Ok(p) = text.parse() {
                            *port = p;
                        }
                    }
                });
                ui.horizontal(|ui| {
                    ui.label("Database:");
                    ui.text_edit_singleline(database);
                });
                ui.horizontal(|ui| {
                    ui.label("User:");
                    ui.text_edit_singleline(user);
                });
                ui.horizontal(|ui| {
                    ui.label("SSL:");
                    egui::ComboBox::from_label("")
                        .selected_text(ssl_mode.label())
                        .show_ui(ui, |ui| {
                            for mode in MysqlSslMode::ALL {
                                ui.selectable_value(ssl_mode, mode, mode.label());
                            }
                        });
                });
                ssh_section(ui, ssh, &mut draft.ssh_password);
            }
        }
        if draft.config.prompts_for_password() {
            ui.horizontal(|ui| {
                ui.label("Password:");
                ui.add(egui::TextEdit::singleline(&mut draft.password).password(true));
            });
        }
        if let Some(error) = state.draft().and_then(|d| d.error.clone()) {
            ui.colored_label(ui.visuals().error_fg_color, error);
        }
        ui.horizontal(|ui| {
            if ui.button("Save and connect").clicked() {
                save = true;
            }
            if ui.button("Cancel").clicked() {
                close = true;
            }
        });
    });
    if save {
        let is_new = state.draft().is_some_and(|d| d.editing.is_none());
        state.save_draft();
        if state.draft().is_none() && is_new {
            if let Some(id) = state.connections().last().map(|c| c.id) {
                state.connect(id);
            }
        }
    } else if close {
        state.cancel_draft();
    }
}

/// The SSH tunnel fields, rendered for Postgres and MySQL drafts only. The
/// password box binds the draft's transient ssh secret, never the config.
fn ssh_section(ui: &mut egui::Ui, ssh: &mut Option<SshConfig>, secret: &mut String) {
    let mut enabled = ssh.is_some();
    if ui.checkbox(&mut enabled, "Use SSH tunnel").changed() {
        *ssh = enabled.then(SshConfig::default);
    }
    let Some(ssh) = ssh.as_mut() else { return };
    ui.horizontal(|ui| {
        ui.label("SSH host:");
        ui.text_edit_singleline(&mut ssh.host);
    });
    ui.horizontal(|ui| {
        ui.label("SSH port:");
        let mut text = ssh.port.to_string();
        if ui.text_edit_singleline(&mut text).changed() {
            if let Ok(port) = text.parse() {
                ssh.port = port;
            }
        }
    });
    ui.horizontal(|ui| {
        ui.label("SSH user:");
        ui.text_edit_singleline(&mut ssh.user);
    });
    ui.horizontal(|ui| {
        ui.label("SSH auth:");
        let password_auth = matches!(ssh.auth, SshAuth::Password);
        if ui.selectable_label(password_auth, "Password").clicked() && !password_auth {
            ssh.auth = SshAuth::Password;
        }
        if ui.selectable_label(!password_auth, "Key file").clicked() && password_auth {
            ssh.auth = SshAuth::KeyFile { path: PathBuf::new(), passphrase: None };
        }
    });
    match &mut ssh.auth {
        SshAuth::Password => {
            ui.horizontal(|ui| {
                ui.label("SSH password:");
                ui.add(egui::TextEdit::singleline(secret).password(true));
            });
        }
        SshAuth::KeyFile { path, passphrase } => {
            ui.horizontal(|ui| {
                ui.label("Key file:");
                let mut text = path.to_string_lossy().into_owned();
                if ui.text_edit_singleline(&mut text).changed() {
                    *path = text.into();
                }
            });
            ui.horizontal(|ui| {
                ui.label("Passphrase:");
                let mut text = passphrase.clone().unwrap_or_default();
                if ui.add(egui::TextEdit::singleline(&mut text).password(true)).changed() {
                    *passphrase = (!text.is_empty()).then_some(text);
                }
            });
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::app::SavedQueryVersion;

    #[test]
    fn version_labels_rank_newest_first_and_truncate() {
        let newest = SavedQueryVersion { at: 2, sql: "SELECT 2".to_owned() };
        let oldest = SavedQueryVersion { at: 1, sql: "SELECT 1".to_owned() };
        assert_eq!(saved_version_label(0, &newest), "v1 · SELECT 2");
        assert_eq!(saved_version_label(1, &oldest), "v2 · SELECT 1");
        let long = SavedQueryVersion {
            at: 3,
            sql: format!("SELECT {}", "x".repeat(40)),
        };
        assert_eq!(saved_version_label(0, &long), format!("v1 · SELECT {}", "x".repeat(21)) + "…");
        let blank = SavedQueryVersion { at: 4, sql: "   ".to_owned() };
        assert_eq!(saved_version_label(2, &blank), "v3 · SQL");
    }

    #[test]
    fn connection_json_postgres_blob_fills_config_password_and_name() {
        let parsed = parse_connection_json(
            r#"{"connectionType":"postgresql","host":"db.internal","port":5433,
                "username":"owner","password":"s3cret","defaultDatabase":"analytics",
                "name":"warehouse"}"#,
        )
        .unwrap();
        assert_eq!(parsed.kind, crate::db::DbKind::Postgres);
        assert_eq!(
            parsed.config,
            ConnectionConfig::Postgres {
                host: "db.internal".to_owned(),
                port: 5433,
                database: "analytics".to_owned(),
                user: "owner".to_owned(),
                ssl_mode: PostgresSslMode::Disable,
                ssh: None,
                read_only: false,
            }
        );
        assert_eq!(parsed.password, Some("s3cret".to_owned()));
        assert_eq!(parsed.name, Some("warehouse".to_owned()));
    }

    #[test]
    fn connection_json_mysql_blob_maps_ssl_and_read_only() {
        let parsed = parse_connection_json(
            r#"{"connectionType":"mysql","host":"db.internal","port":"3307",
                "user":"root","password":"cdepass","database":"cde_test",
                "ssl":true,"readOnlyMode":true}"#,
        )
        .unwrap();
        assert_eq!(parsed.kind, crate::db::DbKind::Mysql);
        assert_eq!(
            parsed.config,
            ConnectionConfig::Mysql {
                host: "db.internal".to_owned(),
                port: 3307,
                database: "cde_test".to_owned(),
                user: "root".to_owned(),
                ssl_mode: MysqlSslMode::Required,
                ssh: None,
                read_only: true,
            }
        );
        assert_eq!(parsed.password, Some("cdepass".to_owned()));
        assert_eq!(parsed.name, None);
    }

    #[test]
    fn connection_json_accepts_aliases_and_kind_defaults() {
        let parsed = parse_connection_json(
            r#"{"connectionType":"psql","username":"owner","defaultDatabase":"appdb"}"#,
        )
        .unwrap();
        assert_eq!(
            parsed.config,
            ConnectionConfig::Postgres {
                host: "localhost".to_owned(),
                port: 5432,
                database: "appdb".to_owned(),
                user: "owner".to_owned(),
                ssl_mode: PostgresSslMode::Disable,
                ssh: None,
                read_only: false,
            }
        );
        assert_eq!(parsed.password, None);
        let parsed = parse_connection_json(
            r#"{"type":"mariadb","host":"db.internal","user":"root","db":"cde_test"}"#,
        )
        .unwrap();
        assert_eq!(parsed.kind, crate::db::DbKind::Mysql);
        let ConnectionConfig::Mysql { port, ssl_mode, .. } = parsed.config else {
            panic!("expected a mysql config");
        };
        assert_eq!(port, 3306);
        assert_eq!(ssl_mode, MysqlSslMode::Preferred);
    }

    #[test]
    fn connection_json_sqlite_blob_reads_every_path_key() {
        for key in ["defaultDatabase", "database", "path", "filename"] {
            let parsed = parse_connection_json(&format!(
                "{{\"connectionType\":\"sqlite\",\"{key}\":\"/tmp/app.db\"}}"
            ))
            .unwrap();
            assert_eq!(parsed.kind, crate::db::DbKind::Sqlite, "{key}");
            assert_eq!(
                parsed.config,
                ConnectionConfig::Sqlite { path: PathBuf::from("/tmp/app.db"), read_only: false },
                "{key}"
            );
            assert_eq!(parsed.password, None, "{key}");
        }
    }

    #[test]
    fn connection_json_url_field_delegates_to_the_url_parser() {
        let parsed = parse_connection_json(
            r#"{"connectionType":"ignored","url":"postgres://owner:s3cret@db.internal:5433/analytics?sslmode=require","name":"warehouse"}"#,
        )
        .unwrap();
        assert_eq!(parsed.kind, crate::db::DbKind::Postgres);
        assert_eq!(
            parsed.config,
            ConnectionConfig::Postgres {
                host: "db.internal".to_owned(),
                port: 5433,
                database: "analytics".to_owned(),
                user: "owner".to_owned(),
                ssl_mode: PostgresSslMode::Require,
                ssh: None,
                read_only: false,
            }
        );
        assert_eq!(parsed.password, Some("s3cret".to_owned()));
        assert_eq!(parsed.name, Some("warehouse".to_owned()));
    }

    #[test]
    fn connection_json_rejects_garbage_unknown_types_and_bad_ports() {
        use crate::db::DbError;
        assert_eq!(
            parse_connection_json(""),
            Err(DbError::Config("invalid connection JSON".to_owned()))
        );
        assert_eq!(
            parse_connection_json("not json"),
            Err(DbError::Config("invalid connection JSON".to_owned()))
        );
        assert_eq!(
            parse_connection_json("[1, 2]"),
            Err(DbError::Config("invalid connection JSON".to_owned()))
        );
        assert_eq!(
            parse_connection_json(r#"{"host":"localhost"}"#),
            Err(DbError::Config("invalid connection JSON".to_owned()))
        );
        assert_eq!(
            parse_connection_json(r#"{"connectionType":"oracle","host":"localhost"}"#),
            Err(DbError::Config("unsupported connection type \"oracle\"".to_owned()))
        );
        assert_eq!(
            parse_connection_json(r#"{"connectionType":"sqlite"}"#),
            Err(DbError::Config("invalid connection JSON".to_owned()))
        );
        assert_eq!(
            parse_connection_json(
                r#"{"connectionType":"postgres","host":"h","port":0,"database":"d","username":"u"}"#
            ),
            Err(DbError::Config("invalid connection JSON".to_owned()))
        );
        assert_eq!(
            parse_connection_json(
                r#"{"connectionType":"postgres","host":"h","port":"nope","database":"d","username":"u"}"#
            ),
            Err(DbError::Config("invalid connection JSON".to_owned()))
        );
    }
}
// Manual check: save SELECT 1 then SELECT 2 under one name, expand History on
// the saved row, press Restore on v2, and a new tab opens with SELECT 1.
// Manual check: type a folder and press Files or Dir under SAVED, and one
// saved query per .sql file lands in that folder; paste a Beekeeper JSON
// blob in the JSON row and Parse fills host, port, user, database, and name.
