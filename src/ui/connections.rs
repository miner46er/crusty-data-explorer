//! Connection sidebar and the new/edit connection modal.

use eframe::egui;

use crate::app::{AppState, ConnectionState};
use crate::db::{ConnectionConfig, ConnectionId};

const ACCENT: egui::Color32 = egui::Color32::from_rgb(58, 110, 165);
const LIVE_GREEN: egui::Color32 = egui::Color32::from_rgb(115, 180, 115);

enum ConnectionStateKind {
    Live,
    Connecting,
    Offline,
}

enum SidebarAction {
    Connect(ConnectionId),
    Disconnect(ConnectionId),
    NewQuery(ConnectionId),
    Edit(ConnectionId),
    Delete(ConnectionId),
    RefreshSchema(ConnectionId),
    PreviewTable(ConnectionId, String),
}

pub fn sidebar(state: &mut AppState, ui: &mut egui::Ui) {
    ui.with_layout(egui::Layout::bottom_up(egui::Align::Min), |ui| {
        if ui
            .add_sized(
                [ui.available_width(), 28.0],
                egui::Button::new(egui::RichText::new("+ New connection").strong()).fill(ACCENT),
            )
            .clicked()
        {
            state.open_draft(None);
        }
        ui.separator();
        egui::ScrollArea::vertical().show(ui, |ui| {
            ui.with_layout(egui::Layout::top_down(egui::Align::LEFT), |ui| {
                ui.label(egui::RichText::new("CONNECTIONS").small().weak());
                ui.add_space(4.0);

                if state.connections().is_empty() {
                    ui.label("No connections yet.");
                    ui.small("Add one below to start querying.");
                    ui.add_space(8.0);
                }

                let active_conn = state
                    .active()
                    .and_then(|id| state.tab(id))
                    .map(|tab| tab.conn);

                let mut actions: Vec<SidebarAction> = Vec::new();
                let snapshot: Vec<(ConnectionId, String, String, bool)> = state
                    .connections()
                    .iter()
                    .map(|c| {
                        let (dot, text) = match c.state() {
                            ConnectionState::Disconnected => ("○", "disconnected".to_owned()),
                            ConnectionState::Connecting { .. } => {
                                ("◌", "connecting".to_owned())
                            }
                            ConnectionState::Connected { session } => {
                                ("●", session.server_version.clone())
                            }
                            ConnectionState::QueryRunning { session, .. } => {
                                ("●", session.server_version.clone())
                            }
                            ConnectionState::Error { message } => ("●", message.clone()),
                        };
                        let live = matches!(
                            c.state(),
                            ConnectionState::Connected { .. }
                                | ConnectionState::QueryRunning { .. }
                        );
                        (c.id, c.name.clone(), format!("{dot} {text}"), live)
                    })
                    .collect();
                let dot_colors: std::collections::HashMap<ConnectionId, egui::Color32> = state
                    .connections()
                    .iter()
                    .map(|c| {
                        let color = match c.state() {
                            ConnectionState::Disconnected => egui::Color32::GRAY,
                            ConnectionState::Connecting { .. } => egui::Color32::YELLOW,
                            ConnectionState::Connected { .. }
                            | ConnectionState::QueryRunning { .. } => LIVE_GREEN,
                            ConnectionState::Error { .. } => egui::Color32::RED,
                        };
                        (c.id, color)
                    })
                    .collect();
                for (conn_id, conn_name, state_label, _is_live) in snapshot {
                    let selected = Some(conn_id) == active_conn;
                    let dot_color =
                        dot_colors.get(&conn_id).copied().unwrap_or(egui::Color32::GRAY);
                    let (dot, state_text) = match state_label.split_once(' ') {
                        Some((d, t)) => (d.to_owned(), t.to_owned()),
                        None => ("○".to_owned(), state_label.clone()),
                    };
                    let frame_fill = if selected {
                        ui.visuals().selection.bg_fill.gamma_multiply(0.35)
                    } else {
                        ui.visuals().faint_bg_color
                    };
                    let frame_stroke = if selected {
                        egui::Stroke::new(1.0, ACCENT)
                    } else {
                        ui.visuals().widgets.noninteractive.bg_stroke
                    };
                    let kind: ConnectionStateKind = {
                        let conn = state
                            .connections()
                            .iter()
                            .find(|c| c.id == conn_id)
                            .map(|c| match c.state() {
                                ConnectionState::Connected { .. } => ConnectionStateKind::Live,
                                ConnectionState::QueryRunning { .. } => ConnectionStateKind::Live,
                                ConnectionState::Connecting { .. } => {
                                    ConnectionStateKind::Connecting
                                }
                                ConnectionState::Disconnected => ConnectionStateKind::Offline,
                                ConnectionState::Error { .. } => ConnectionStateKind::Offline,
                            })
                            .unwrap_or(ConnectionStateKind::Offline);
                        conn
                    };
                    egui::Frame::group(ui.style())
                        .fill(frame_fill)
                        .stroke(frame_stroke)
                        .corner_radius(egui::CornerRadius::same(6))
                        .inner_margin(egui::Margin::same(8))
                        .show(ui, |ui| {
                            ui.set_min_width(ui.available_width() - 16.0);
                            ui.horizontal(|ui| {
                                ui.colored_label(dot_color, &dot);
                                ui.vertical(|ui| {
                                    ui.label(egui::RichText::new(&conn_name).strong());
                                    ui.label(
                                        egui::RichText::new(&state_text).small().weak(),
                                    );
                                });
                            });
                            ui.add_space(4.0);
                            ui.horizontal(|ui| {
                                match kind {
                                    ConnectionStateKind::Live => {
                                        if ui
                                            .add(
                                                egui::Button::new(
                                                    egui::RichText::new("New query")
                                                        .small()
                                                        .strong(),
                                                )
                                                .fill(ACCENT)
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
                                    ConnectionStateKind::Connecting => {
                                        if ui.small_button("Stop").clicked() {
                                            actions.push(SidebarAction::Disconnect(conn_id));
                                        }
                                    }
                                    ConnectionStateKind::Offline => {
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
                            if matches!(kind, ConnectionStateKind::Live) {
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
                                        super::schema::SchemaAction::Preview(id, table) => {
                                            actions.push(SidebarAction::PreviewTable(id, table));
                                        }
                                    }
                                }
                            }
                        });
                    ui.add_space(4.0);
                }

                for action in actions {
                    match action {
                        SidebarAction::Connect(id) => state.connect(id),
                        SidebarAction::Disconnect(id) => state.disconnect(id),
                        SidebarAction::NewQuery(id) => {
                            let tab = state.new_tab(id);
                            state.activate(tab);
                        }
                        SidebarAction::Edit(id) => state.open_draft(Some(id)),
                        SidebarAction::Delete(id) => state.delete_connection(id),
                        SidebarAction::RefreshSchema(id) => state.refresh_schema(id),
                        SidebarAction::PreviewTable(id, table) => state.preview_table(id, &table),
                    }
                }
            });
        });
    });
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
        ui.horizontal(|ui| {
            ui.label("Name:");
            ui.text_edit_singleline(&mut draft.name);
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
        match &mut draft.config {
            ConnectionConfig::Sqlite { path } => {
                ui.horizontal(|ui| {
                    ui.label("Path:");
                    let mut text = path.to_string_lossy().into_owned();
                    if ui.text_edit_singleline(&mut text).changed() {
                        *path = text.into();
                    }
                });
                ui.small("Use :memory: for a throwaway database.");
            }
            ConnectionConfig::Postgres { host, port, database, user } => {
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
            }
        }
        if matches!(draft.config, ConnectionConfig::Postgres { .. }) {
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
