//! Connection sidebar and the new/edit connection modal.

use eframe::egui;

use crate::app::{AppState, ConnectionState};
use crate::db::{ConnectionConfig, ConnectionId};

enum SidebarAction {
    Connect(ConnectionId),
    Disconnect(ConnectionId),
    NewQuery(ConnectionId),
    Edit(ConnectionId),
    Delete(ConnectionId),
}

pub fn sidebar(state: &mut AppState, ui: &mut egui::Ui) {
    ui.heading("Connections");
    ui.separator();

    let mut actions: Vec<SidebarAction> = Vec::new();
    for conn in state.connections() {
        let (dot, dot_color, state_text) = match conn.state() {
            ConnectionState::Disconnected => ("○", egui::Color32::GRAY, "disconnected"),
            ConnectionState::Connecting { .. } => ("◌", egui::Color32::YELLOW, "connecting"),
            ConnectionState::Connected { session } => {
                ("●", egui::Color32::GREEN, session.server_version.as_str())
            }
            ConnectionState::QueryRunning { session, .. } => {
                ("●", egui::Color32::LIGHT_BLUE, session.server_version.as_str())
            }
            ConnectionState::Error { message } => ("●", egui::Color32::RED, message.as_str()),
        };
        ui.horizontal(|ui| {
            ui.colored_label(dot_color, dot);
            ui.label(&conn.name);
        });
        ui.horizontal(|ui| {
            ui.small(state_text);
        });
        ui.horizontal(|ui| {
            match conn.state() {
                ConnectionState::Connected { .. } | ConnectionState::QueryRunning { .. } => {
                    if ui.small_button("Disconnect").clicked() {
                        actions.push(SidebarAction::Disconnect(conn.id));
                    }
                    if ui.small_button("New query").clicked() {
                        actions.push(SidebarAction::NewQuery(conn.id));
                    }
                }
                ConnectionState::Connecting { .. } => {
                    if ui.small_button("Stop").clicked() {
                        actions.push(SidebarAction::Disconnect(conn.id));
                    }
                }
                ConnectionState::Disconnected | ConnectionState::Error { .. } => {
                    if ui.small_button("Connect").clicked() {
                        actions.push(SidebarAction::Connect(conn.id));
                    }
                }
            }
            if ui.small_button("Edit").clicked() {
                actions.push(SidebarAction::Edit(conn.id));
            }
            if ui.small_button("Delete").clicked() {
                actions.push(SidebarAction::Delete(conn.id));
            }
        });
        ui.separator();
    }

    if ui.button("+ New connection").clicked() {
        state.open_draft(None);
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
        }
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
